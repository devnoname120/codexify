use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use codexify::agent_automation::{AgentAutomationStore, GoalStatus, PAIRING_REFERENCE_TTL_MS};
use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::markdown_chat::MarkdownChatStore;
use codexify::project_bindings::ConversationIdentity;
use codexify::tool::{Tool, ToolRequestContext};
use codexify::tools::automation_pair::AutomationVerifyPairing;
use codexify::tools::get_agent_brief::GetAgentBrief;
use codexify::tools::goal::GoalTool;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Fixture {
    _root: tempfile::TempDir,
    config: codexify::types::AppConfig,
    session: SessionState,
    task: ConversationIdentity,
    automation: Arc<AgentAutomationStore>,
    context: ToolRequestContext,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn fixture(name: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let mut config = default_config(root.path().to_path_buf());
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.task_state_root = Some(root.path().join("metadata"));
    config.markdown_chat.enabled = true;
    config.markdown_chat.automation.enabled = true;
    config
        .markdown_chat
        .automation
        .retry_timings
        .infinite_retries = false;
    let task = ConversationIdentity::from_openai_session(name).unwrap();
    let automation = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
    let context = context(
        root.path(),
        Some(task.clone()),
        Some(task.clone()),
        Some(0),
        Some(automation.clone()),
    );
    Fixture {
        _root: root,
        config,
        session: SessionState::new(),
        task,
        automation,
        context,
    }
}

fn context(
    root: &std::path::Path,
    conversation: Option<ConversationIdentity>,
    task_conversation: Option<ConversationIdentity>,
    task_owner_generation: Option<u64>,
    agent_automation: Option<Arc<AgentAutomationStore>>,
) -> ToolRequestContext {
    ToolRequestContext {
        conversation,
        task_conversation,
        task_owner_generation,
        conversation_retired: false,
        connector_schema_version: None,
        conversation_schema_version: None,
        conversation_authorizations: Arc::new(
            codexify::conversation_auth::ConversationAuthorizationStore::new(),
        ),
        project_bindings: Arc::new(codexify::project_bindings::ProjectBindingStore::new(
            root.join("bindings"),
        )),
        diff_checkpoints: Arc::new(codexify::diff::DiffCheckpointManager::new()),
        artifact_egress: Arc::new(codexify::artifact_egress::ArtifactEgressStore::new_at(
            codexify::types::ArtifactEgressConfig::default(),
            root.join("artifacts"),
        )),
        markdown_chat: Arc::new(MarkdownChatStore::default()),
        agent_automation,
        agent_runtime: None,
        cancellation: CancellationToken::new(),
    }
}

fn payload(result: &codexify::types::ToolResult) -> &Value {
    result.structured_content.as_ref().unwrap()
}

#[tokio::test]
#[ignore = "requires CODEXIFY_TEST_APPRISE_PYTHON; local notification receiver only"]
async fn goal_finish_notifies_once_and_filters_blocked_and_completed_independently() {
    use axum::{Router, routing::post};
    let mut f = fixture("goal-notifications");
    let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/",
        post(move |body: String| {
            let sender = sender.clone();
            async move {
                sender
                    .send(serde_json::from_str::<Value>(&body).unwrap())
                    .unwrap();
                axum::Json(json!({"id":"test","event":"message"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    f.config.markdown_chat.notifications = Some(
        serde_json::from_value(json!({
            "urls":[format!("ntfy://{address}/test?image=no")],
            "pythonPath":std::env::var("CODEXIFY_TEST_APPRISE_PYTHON").unwrap(),
            "events":{"messages":false,"offline":false}
        }))
        .unwrap(),
    );
    f.automation = Arc::new(AgentAutomationStore::for_config(&f.config).unwrap());
    f.context.agent_automation = Some(f.automation.clone());
    let goal = f
        .automation
        .set_goal(&f.task, "Complete the task", 1)
        .unwrap();
    for (terminal, detail) in [
        ("blocked", "Need a decision"),
        ("completed", "Task finished"),
    ] {
        f.automation
            .run_goal(&f.task, goal.revision, now_ms())
            .unwrap();
        let mut args = json!({"expectedRevision":goal.revision,"status":terminal,"nextSteps":"Review the report and provide any needed input"});
        args[if terminal == "blocked" {
            "reason"
        } else {
            "summary"
        }] = json!(detail);
        let result = GoalTool::Finish
            .call_with_context(args.clone(), &f.config, &f.session, &f.context)
            .await;
        assert!(!result.is_error, "{}", result.joined_text());
        let alert = tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(alert["message"].as_str().unwrap().contains(detail));
        GoalTool::Finish
            .call_with_context(args, &f.config, &f.session, &f.context)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            received.try_recv().is_err(),
            "replayed goal_finish must not notify again"
        );
    }
    let events = &mut f
        .config
        .markdown_chat
        .notifications
        .as_mut()
        .unwrap()
        .events;
    events.goal_blocked = false;
    events.completed = false;
    f.context.agent_automation = Some(Arc::new(
        AgentAutomationStore::for_config(&f.config).unwrap(),
    ));
    for terminal in ["blocked", "completed"] {
        let goal = f
            .automation
            .set_goal(&f.task, "Another task", now_ms())
            .unwrap();
        f.automation
            .run_goal(&f.task, goal.revision, now_ms())
            .unwrap();
        let mut args = json!({"expectedRevision":goal.revision,"status":terminal,"nextSteps":"Review the report and provide any needed input"});
        args[if terminal == "blocked" {
            "reason"
        } else {
            "summary"
        }] = json!("Filtered outcome");
        let result = GoalTool::Finish
            .call_with_context(args, &f.config, &f.session, &f.context)
            .await;
        assert!(!result.is_error, "{}", result.joined_text());
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(received.try_recv().is_err());
    server.abort();
}

#[test]
fn tool_contracts_are_closed_bounded_and_classified() {
    let pair = AutomationVerifyPairing;
    let read = GoalTool::Read;
    let finish = GoalTool::Finish;
    let set = GoalTool::Set;
    let resume = GoalTool::Resume;

    assert!(!pair.behavior().read_only);
    assert!(read.behavior().read_only);
    assert!(!finish.behavior().read_only);
    assert!(!set.behavior().read_only && set.behavior().idempotent);
    assert!(!resume.behavior().read_only && resume.behavior().idempotent);
    assert_eq!(
        pair.input_schema()["properties"]["reference"]["minLength"],
        43
    );
    assert_eq!(
        pair.input_schema()["properties"]["reference"]["maxLength"],
        43
    );
    assert_eq!(
        pair.input_schema()["properties"]["reference"]["writeOnly"],
        true
    );
    for tool in [&pair as &dyn Tool, &read, &set, &resume, &finish] {
        assert_eq!(tool.input_schema()["additionalProperties"], false);
        let output = tool
            .output_schema()
            .expect("automation tools declare output schemas");
        assert_eq!(output["type"], "object");
        assert_eq!(output["additionalProperties"], false);
        jsonschema::validator_for(&tool.input_schema()).unwrap();
        jsonschema::validator_for(&output).unwrap();
    }
}

#[tokio::test]
async fn disabled_automation_returns_an_explicit_error_without_state_access() {
    let mut fixture = fixture("disabled");
    fixture.config.markdown_chat.automation.enabled = false;
    fixture.context.agent_automation = None;

    for (tool, args) in [
        (&GoalTool::Read as &dyn Tool, json!({})),
        (
            &GoalTool::Set as &dyn Tool,
            json!({"requestId":"set","expectedRevision":0,"markdown":"Goal"}),
        ),
        (
            &GoalTool::Resume as &dyn Tool,
            json!({"requestId":"resume","expectedRevision":1}),
        ),
        (
            &GoalTool::Finish as &dyn Tool,
            json!({"expectedRevision":1,"status":"completed"}),
        ),
        (
            &AutomationVerifyPairing as &dyn Tool,
            json!({"reference":"a".repeat(43)}),
        ),
    ] {
        let result = tool
            .call_with_context(args, &fixture.config, &fixture.session, &fixture.context)
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("disabled"));
    }
}

#[tokio::test]
async fn automation_tools_require_physical_and_stable_task_identity() {
    let fixture = fixture("missing-context");
    let missing = context(
        fixture._root.path(),
        None,
        None,
        None,
        Some(fixture.automation.clone()),
    );

    for (tool, args) in [
        (&GoalTool::Read as &dyn Tool, json!({})),
        (
            &GoalTool::Set as &dyn Tool,
            json!({"requestId":"set","expectedRevision":0,"markdown":"Goal"}),
        ),
        (
            &GoalTool::Resume as &dyn Tool,
            json!({"requestId":"resume","expectedRevision":1}),
        ),
        (
            &GoalTool::Finish as &dyn Tool,
            json!({"expectedRevision":1,"status":"completed"}),
        ),
        (
            &AutomationVerifyPairing as &dyn Tool,
            json!({"reference":"a".repeat(43)}),
        ),
    ] {
        let result = tool
            .call_with_context(args, &fixture.config, &fixture.session, &missing)
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("stable ChatGPT conversation"));
    }
}

#[tokio::test]
async fn goal_set_activates_updates_and_retries_without_chat_messages() {
    let f = fixture("agent-goal-set");
    let chat = f
        .context
        .markdown_chat
        .chat(&f.config, Some(&f.task), &f.session)
        .unwrap();
    chat.ensure().await.unwrap();
    let original_chat = std::fs::read(chat.path()).unwrap();
    chat.record_agent_call(now_ms()).await.unwrap();
    chat.request_agent_stop("stop-before-set".into())
        .await
        .unwrap();
    let create = json!({"requestId":"create","expectedRevision":0,"markdown":"Initial goal"});
    let result = GoalTool::Set
        .call_with_context(create.clone(), &f.config, &f.session, &f.context)
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    assert_eq!(payload(&result)["status"], "active");
    assert_eq!(payload(&result)["revision"], 1);
    assert_eq!(payload(&result)["terminal"], false);
    assert!(!chat.agent_stop_requested().await.unwrap());
    let before = f.automation.load(&f.task).unwrap().goal.unwrap();

    let restarted = AgentAutomationStore::new(f._root.path().join("metadata/chats")).unwrap();
    let mut context = f.context.clone();
    context.agent_automation = Some(Arc::new(restarted));
    let replay = GoalTool::Set
        .call_with_context(create, &f.config, &f.session, &context)
        .await;
    assert!(!replay.is_error, "{}", replay.joined_text());
    assert_eq!(payload(&replay)["revision"], 1);

    let edit = json!({"requestId":"edit","expectedRevision":1,"markdown":"Updated goal"});
    let edited = GoalTool::Set
        .call_with_context(edit.clone(), &f.config, &f.session, &f.context)
        .await;
    assert!(!edited.is_error, "{}", edited.joined_text());
    let after = f.automation.load(&f.task).unwrap().goal.unwrap();
    assert_eq!(after.revision, 2);
    assert_eq!(after.markdown, "Updated goal");
    assert_eq!(after.status, GoalStatus::Active);
    assert_eq!(after.created_at_ms, before.created_at_ms);
    assert_eq!(after.elapsed_active_ms, before.elapsed_active_ms);
    assert_eq!(after.turn_count, before.turn_count);

    f.automation.stop_goal(&f.task, 2, now_ms()).unwrap();
    chat.request_agent_stop("owner-stop-after-edit".into())
        .await
        .unwrap();
    let paused = f.automation.load(&f.task).unwrap().goal.unwrap();
    let replay = GoalTool::Set
        .call_with_context(edit, &f.config, &f.session, &f.context)
        .await;
    assert!(!replay.is_error, "{}", replay.joined_text());
    assert_eq!(
        payload(&replay)["status"],
        "configured",
        "old retry must return current state"
    );
    assert_eq!(f.automation.load(&f.task).unwrap().goal.unwrap(), paused);
    assert!(
        chat.agent_stop_requested().await.unwrap(),
        "old retry must not undo an owner stop"
    );
    assert_eq!(
        std::fs::read(chat.path()).unwrap(),
        original_chat,
        "goal control must not fabricate user messages"
    );
}

#[tokio::test]
async fn goal_resume_preserves_paused_and_blocked_progress_and_refuses_completed() {
    use codexify::agent_automation::{GoalFinish, OwnerGoalCommand};
    let f = fixture("agent-goal-resume");
    let chat = f
        .context
        .markdown_chat
        .chat(&f.config, Some(&f.task), &f.session)
        .unwrap();
    chat.ensure().await.unwrap();
    f.automation
        .apply_owner_goal_command(
            &f.task,
            "initial",
            OwnerGoalCommand::Set {
                markdown: "Keep this goal".into(),
            },
            1000,
        )
        .unwrap();
    f.automation
        .sync_goal_presence(&f.task, Some(1000), None, false, 1000)
        .unwrap();
    f.automation.stop_goal(&f.task, 1, 6000).unwrap();
    let paused = f.automation.load(&f.task).unwrap().goal.unwrap();
    assert_eq!(paused.elapsed_active_ms, 5000);
    chat.request_agent_stop("pause".into()).await.unwrap();
    let resume = json!({"requestId":"resume-paused","expectedRevision":1});
    let result = GoalTool::Resume
        .call_with_context(resume.clone(), &f.config, &f.session, &f.context)
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    let active = f.automation.load(&f.task).unwrap().goal.unwrap();
    assert_eq!(active.status, GoalStatus::Active);
    assert_eq!(active.markdown, paused.markdown);
    assert_eq!(active.revision, paused.revision);
    assert_eq!(active.elapsed_active_ms, paused.elapsed_active_ms);
    assert_eq!(active.turn_count, paused.turn_count);
    assert!(!chat.agent_stop_requested().await.unwrap());
    let replay = GoalTool::Resume
        .call_with_context(resume, &f.config, &f.session, &f.context)
        .await;
    assert!(!replay.is_error);
    assert_eq!(f.automation.load(&f.task).unwrap().goal.unwrap(), active);

    f.automation
        .finish_goal(
            &f.task,
            1,
            GoalFinish::Blocked {
                reason: "Need owner decision".into(),
            },
            now_ms(),
        )
        .unwrap();
    let result = GoalTool::Resume
        .call_with_context(
            json!({"requestId":"resume-blocked","expectedRevision":1}),
            &f.config,
            &f.session,
            &f.context,
        )
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    assert_eq!(payload(&result)["status"], "active");
    assert!(payload(&result)["blockedReason"].is_null());
    assert_eq!(
        f.automation
            .load(&f.task)
            .unwrap()
            .goal
            .unwrap()
            .elapsed_active_ms,
        5000
    );
    // A fresh operation on an active goal also preserves the current revision.
    let result = GoalTool::Resume
        .call_with_context(
            json!({"requestId":"resume-active","expectedRevision":1}),
            &f.config,
            &f.session,
            &f.context,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(payload(&result)["revision"], 1);

    f.automation
        .finish_goal(
            &f.task,
            1,
            GoalFinish::Completed { summary: None },
            now_ms(),
        )
        .unwrap();
    let result = GoalTool::Resume
        .call_with_context(
            json!({"requestId":"resume-completed","expectedRevision":1}),
            &f.config,
            &f.session,
            &f.context,
        )
        .await;
    assert!(result.is_error);
    assert_eq!(
        f.automation.load(&f.task).unwrap().goal.unwrap().status,
        GoalStatus::Completed
    );
    let replacement = GoalTool::Set
        .call_with_context(
            json!({"requestId":"replace-completed","expectedRevision":1,"markdown":"New goal"}),
            &f.config,
            &f.session,
            &f.context,
        )
        .await;
    assert!(!replacement.is_error, "{}", replacement.joined_text());
    assert_eq!(payload(&replacement)["revision"], 2);
    assert_eq!(payload(&replacement)["status"], "active");
    assert_eq!(
        f.automation
            .load(&f.task)
            .unwrap()
            .goal
            .unwrap()
            .elapsed_active_ms,
        0
    );
}

#[tokio::test]
async fn goal_control_rejects_stale_invalid_and_reused_requests_without_mutation() {
    let f = fixture("agent-goal-validation");
    let result = GoalTool::Set
        .call_with_context(
            json!({"requestId":"first","expectedRevision":0,"markdown":"Original"}),
            &f.config,
            &f.session,
            &f.context,
        )
        .await;
    assert!(!result.is_error);
    let original = f.automation.load(&f.task).unwrap();
    for (tool, args) in [
        (
            GoalTool::Set,
            json!({"requestId":"stale","expectedRevision":0,"markdown":"Wrong"}),
        ),
        (
            GoalTool::Resume,
            json!({"requestId":"stale-resume","expectedRevision":2}),
        ),
        (
            GoalTool::Set,
            json!({"requestId":"first","expectedRevision":1,"markdown":"Reused ID"}),
        ),
        (
            GoalTool::Resume,
            json!({"requestId":"first","expectedRevision":1}),
        ),
        (
            GoalTool::Set,
            json!({"requestId":"empty","expectedRevision":1,"markdown":" "}),
        ),
        (
            GoalTool::Set,
            json!({"requestId":"large","expectedRevision":1,"markdown":"a".repeat(10*1024+1)}),
        ),
        (
            GoalTool::Set,
            json!({"requestId":"","expectedRevision":1,"markdown":"Invalid ID"}),
        ),
        (
            GoalTool::Set,
            json!({"requestId":"bad\nID","expectedRevision":1,"markdown":"Invalid ID"}),
        ),
        (
            GoalTool::Set,
            json!({"requestId":"missing-revision","markdown":"No revision"}),
        ),
        (
            GoalTool::Resume,
            json!({"requestId":"unknown-field","expectedRevision":1,"markdown":"ignored?"}),
        ),
    ] {
        let result = tool
            .call_with_context(args, &f.config, &f.session, &f.context)
            .await;
        assert!(result.is_error, "{} unexpectedly succeeded", tool.name());
        assert_eq!(f.automation.load(&f.task).unwrap(), original);
    }
    let other = ConversationIdentity::from_openai_session("other-goal").unwrap();
    assert!(f.automation.load(&other).unwrap().goal.is_none());
}

#[tokio::test]
async fn goal_read_and_finish_use_the_exact_stable_task_revision() {
    let fixture = fixture("goal-success");
    let goal = fixture
        .automation
        .set_goal(&fixture.task, "Ship the browser fallback", 1000)
        .unwrap();
    fixture
        .automation
        .run_goal(&fixture.task, goal.revision, 1001)
        .unwrap();

    let read = GoalTool::Read
        .call_with_context(
            json!({}),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(!read.is_error, "{}", read.joined_text());
    assert_eq!(payload(&read)["status"], "active");
    assert_eq!(payload(&read)["revision"], goal.revision);
    assert_eq!(payload(&read)["markdown"], "Ship the browser fallback");
    assert_eq!(payload(&read)["terminal"], false);

    let finish_args = json!({
        "expectedRevision": goal.revision,
        "status": "completed",
        "summary": "Implemented and verified",
        "nextSteps": "Deploy after review"
    });
    let finished = GoalTool::Finish
        .call_with_context(
            finish_args.clone(),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(!finished.is_error, "{}", finished.joined_text());
    assert_eq!(payload(&finished)["status"], "completed");
    assert!(
        payload(&finished)["summary"]
            .as_str()
            .unwrap()
            .contains("Implemented and verified")
    );
    assert!(
        payload(&finished)["summary"]
            .as_str()
            .unwrap()
            .contains("Deploy after review")
    );
    assert_eq!(payload(&finished)["assistant_turn_may_end"], false);
    assert_eq!(payload(&finished)["required_next_action"], "turn_completed");
    assert!(
        finished
            .joined_text()
            .contains("Required next call: turn_completed")
    );
    assert!(jsonschema::is_valid(
        &GoalTool::Finish.output_schema().unwrap(),
        payload(&finished)
    ));
    assert_eq!(payload(&finished)["terminal"], true);

    let replay = GoalTool::Finish
        .call_with_context(
            finish_args,
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(!replay.is_error, "{}", replay.joined_text());
    assert_eq!(payload(&replay), payload(&finished));

    let stale = GoalTool::Finish
        .call_with_context(
            json!({"expectedRevision":goal.revision + 1,"status":"completed","summary":"Done","nextSteps":"None","nextSteps":"None"}),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(stale.is_error);
    assert_eq!(
        fixture
            .automation
            .load(&fixture.task)
            .unwrap()
            .goal
            .unwrap()
            .status,
        GoalStatus::Completed
    );
}

#[tokio::test]
async fn blocked_goal_requires_a_concrete_reason() {
    let fixture = fixture("goal-blocked");
    let goal = fixture
        .automation
        .set_goal(&fixture.task, "Wait for unavailable hardware", 1000)
        .unwrap();
    fixture
        .automation
        .run_goal(&fixture.task, goal.revision, 1001)
        .unwrap();

    for args in [
        json!({"expectedRevision":goal.revision,"status":"blocked","nextSteps":"Connect the device"}),
        json!({"expectedRevision":goal.revision,"status":"blocked","reason":"   ","nextSteps":"Connect the device"}),
    ] {
        let result = GoalTool::Finish
            .call_with_context(args, &fixture.config, &fixture.session, &fixture.context)
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("concrete reason"));
    }

    let blocked = GoalTool::Finish
        .call_with_context(
            json!({
                "expectedRevision":goal.revision,
                "status":"blocked",
                "reason":"The required device is not connected",
                "nextSteps":"Connect the device and confirm it is reachable"
            }),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(!blocked.is_error, "{}", blocked.joined_text());
    assert_eq!(payload(&blocked)["status"], "blocked");
    let report = payload(&blocked)["blockedReason"].as_str().unwrap();
    assert!(report.contains("The required device is not connected"));
    assert!(report.contains("Connect the device and confirm it is reachable"));
    assert_eq!(payload(&blocked)["assistant_turn_may_end"], false);
    assert_eq!(payload(&blocked)["required_next_action"], "chat_await");
    assert!(
        blocked
            .joined_text()
            .contains("Required next call: chat_await")
    );
    assert!(jsonschema::is_valid(
        &GoalTool::Finish.output_schema().unwrap(),
        payload(&blocked)
    ));
    assert_eq!(payload(&blocked)["terminal"], true);
}

#[tokio::test]
async fn goal_finish_requires_complete_reports_before_changing_state_and_keeps_them_inline() {
    let f = fixture("goal-reports");
    let chat = f
        .context
        .markdown_chat
        .chat(&f.config, Some(&f.task), &f.session)
        .unwrap();
    chat.ensure().await.unwrap();
    chat.record_agent_call(now_ms()).await.unwrap();
    chat.append_user("pending-report".into(), "Keep this unread".into())
        .await
        .unwrap();
    let unread = chat.unread_snapshot().await.unwrap();
    let goal = f
        .automation
        .set_goal(&f.task, "Verify the deployment", now_ms())
        .unwrap();
    f.automation
        .run_goal(&f.task, goal.revision, now_ms())
        .unwrap();
    let schema = GoalTool::Finish.input_schema();
    for args in [
        json!({"expectedRevision":goal.revision,"status":"completed","nextSteps":"Review"}),
        json!({"expectedRevision":goal.revision,"status":"completed","summary":"Done"}),
        json!({"expectedRevision":goal.revision,"status":"blocked","reason":"Device offline"}),
        json!({"expectedRevision":goal.revision,"status":"blocked","nextSteps":"Connect device"}),
        json!({"expectedRevision":goal.revision,"status":"completed","summary":"   ","nextSteps":"Review"}),
        json!({"expectedRevision":goal.revision,"status":"blocked","reason":"Device offline","nextSteps":"   "}),
        json!({"expectedRevision":goal.revision,"status":"completed","summary":"x".repeat(4096),"nextSteps":"Review"}),
        json!({"expectedRevision":goal.revision,"status":"blocked","reason":"x".repeat(4096),"nextSteps":"Connect device"}),
        json!({"expectedRevision":goal.revision,"status":"completed","summary":"Done","reason":"Wrong field","nextSteps":"Review"}),
    ] {
        let result = GoalTool::Finish
            .call_with_context(args, &f.config, &f.session, &f.context)
            .await;
        assert!(result.is_error, "invalid report was accepted");
        assert_eq!(
            f.automation.load(&f.task).unwrap().goal.unwrap().status,
            GoalStatus::Active
        );
        assert!(
            !chat
                .widget_page(None, None)
                .await
                .unwrap()
                .messages
                .iter()
                .any(|m| m.role == "goal")
        );
    }
    for (outcome, field, detail, next) in [
        (
            "blocked",
            "reason",
            "Device is offline",
            "Reconnect the device",
        ),
        (
            "blocked",
            "reason",
            "Need a test account",
            "Supply a test account",
        ),
        (
            "completed",
            "summary",
            "Deployed and passed all checks",
            "None; deployment is ready",
        ),
    ] {
        // A resumed revision can block again; each distinct finish stays visible.
        f.automation
            .run_goal(&f.task, goal.revision, now_ms())
            .unwrap();
        let mut args = json!({"expectedRevision":goal.revision,"status":outcome,"nextSteps":next});
        args[field] = json!(detail);
        assert!(jsonschema::is_valid(&schema, &args));
        let result = GoalTool::Finish
            .call_with_context(args.clone(), &f.config, &f.session, &f.context)
            .await;
        assert!(!result.is_error, "{}", result.joined_text());
        assert_eq!(payload(&result)["assistant_turn_may_end"], false);
        let retry = GoalTool::Finish
            .call_with_context(args, &f.config, &f.session, &f.context)
            .await;
        assert!(!retry.is_error, "{}", retry.joined_text());
        assert_eq!(payload(&result), payload(&retry));
        assert_eq!(chat.unread_snapshot().await.unwrap(), unread);
        // Guarantee a distinct transition timestamp without relying on millisecond timing.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let reopened = codexify::markdown_chat::MarkdownChatStore::default()
        .chat(&f.config, Some(&f.task), &f.session)
        .unwrap();
    let page = reopened.widget_page(None, None).await.unwrap();
    let events = page
        .messages
        .iter()
        .filter(|m| m.role == "goal")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 3, "retry receipts must not duplicate reports");
    for (event, text, next) in [
        (events[0], "Device is offline", "Reconnect the device"),
        (events[1], "Need a test account", "Supply a test account"),
        (
            events[2],
            "Deployed and passed all checks",
            "None; deployment is ready",
        ),
    ] {
        let report = event.goal_report.as_deref().unwrap();
        assert!(report.contains(text) && report.contains(next));
        assert_eq!(event.start, unread.end);
        assert_eq!(event.tool_call_count, Some(1));
    }
    assert_eq!(events[0].goal_status, Some(GoalStatus::Blocked));
    assert_eq!(events[2].goal_status, Some(GoalStatus::Completed));
}

#[tokio::test]
async fn pairing_verification_consumes_the_reference_and_discloses_no_private_values() {
    let fixture = fixture("pair-success");
    let issued = fixture
        .automation
        .issue_pairing(
            &fixture.task,
            "https://chatgpt.com/c/private-id",
            0,
            now_ms(),
        )
        .unwrap();
    fixture
        .automation
        .record_pairing_canonical_url(
            &fixture.task,
            issued.generation,
            "https://chatgpt.com/c/private-id",
            now_ms(),
        )
        .unwrap();

    let verified = AutomationVerifyPairing
        .call_with_context(
            json!({"reference":issued.reference}),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(!verified.is_error, "{}", verified.joined_text());
    assert_eq!(payload(&verified)["verified"], true);
    assert_eq!(payload(&verified)["pairingGeneration"], issued.generation);
    let visible = format!("{} {}", verified.joined_text(), payload(&verified));
    assert!(!visible.contains("private-id"));

    let state = fixture.automation.load(&fixture.task).unwrap();
    assert_eq!(state.pairing.verified_owner_generation, Some(0));
    assert!(state.pairing.pending.is_none());

    let replay = AutomationVerifyPairing
        .call_with_context(
            json!({"reference":issued.reference}),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(replay.is_error);
}

#[tokio::test]
async fn pairing_mismatch_consumes_the_intended_tasks_pending_reference() {
    let fixture = fixture("pair-owner");
    let issued = fixture
        .automation
        .issue_pairing(&fixture.task, "https://chatgpt.com/c/owner", 0, now_ms())
        .unwrap();
    fixture
        .automation
        .record_pairing_canonical_url(
            &fixture.task,
            issued.generation,
            "https://chatgpt.com/c/owner",
            now_ms(),
        )
        .unwrap();
    let other = ConversationIdentity::from_openai_session("pair-other").unwrap();
    let wrong_context = context(
        fixture._root.path(),
        Some(other.clone()),
        Some(other),
        Some(0),
        Some(fixture.automation.clone()),
    );

    let mismatch = AutomationVerifyPairing
        .call_with_context(
            json!({"reference":issued.reference}),
            &fixture.config,
            &fixture.session,
            &wrong_context,
        )
        .await;
    assert!(mismatch.is_error);
    assert!(
        fixture
            .automation
            .load(&fixture.task)
            .unwrap()
            .pairing
            .pending
            .is_none()
    );

    let retry = AutomationVerifyPairing
        .call_with_context(
            json!({"reference":issued.reference}),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(retry.is_error);
}

#[tokio::test]
async fn pairing_mismatch_across_project_state_roots_consumes_the_reference() {
    let root = tempfile::tempdir().unwrap();
    let index = root.path().join("global-pairings");
    let owner_store = Arc::new(
        AgentAutomationStore::new_with_pairing_index(
            root.path().join("owner/chats"),
            index.clone(),
        )
        .unwrap(),
    );
    let other_store = Arc::new(
        AgentAutomationStore::new_with_pairing_index(root.path().join("other/chats"), index)
            .unwrap(),
    );
    let owner = ConversationIdentity::from_openai_session("cross-root-owner").unwrap();
    let other = ConversationIdentity::from_openai_session("cross-root-other").unwrap();
    let issued = owner_store
        .issue_pairing(&owner, "https://chatgpt.com/c/cross-root", 0, now_ms())
        .unwrap();
    owner_store
        .record_pairing_canonical_url(
            &owner,
            issued.generation,
            "https://chatgpt.com/c/cross-root",
            now_ms(),
        )
        .unwrap();
    let mut config = default_config(root.path().to_path_buf());
    config.markdown_chat.enabled = true;
    config.markdown_chat.automation.enabled = true;
    let wrong_context = context(
        root.path(),
        Some(other.clone()),
        Some(other),
        Some(0),
        Some(other_store),
    );

    let mismatch = AutomationVerifyPairing
        .call_with_context(
            json!({"reference":issued.reference}),
            &config,
            &SessionState::new(),
            &wrong_context,
        )
        .await;

    assert!(mismatch.is_error);
    assert!(owner_store.load(&owner).unwrap().pairing.pending.is_none());
}

#[tokio::test]
async fn expired_pairing_reference_is_consumed() {
    let fixture = fixture("pair-expired");
    let current = now_ms();
    let issued_at = current.saturating_sub(PAIRING_REFERENCE_TTL_MS + 10);
    let issued = fixture
        .automation
        .issue_pairing(&fixture.task, "https://chatgpt.com/c/expired", 0, issued_at)
        .unwrap();
    fixture
        .automation
        .record_pairing_canonical_url(
            &fixture.task,
            issued.generation,
            "https://chatgpt.com/c/expired",
            issued_at + 1,
        )
        .unwrap();

    let result = AutomationVerifyPairing
        .call_with_context(
            json!({"reference":issued.reference}),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(result.is_error);
    assert!(
        fixture
            .automation
            .load(&fixture.task)
            .unwrap()
            .pairing
            .pending
            .is_none()
    );
}

#[tokio::test]
async fn changed_owner_generation_invalidates_and_consumes_pairing() {
    let fixture = fixture("pair-owner-generation");
    let issued = fixture
        .automation
        .issue_pairing(
            &fixture.task,
            "https://chatgpt.com/c/owner-generation",
            3,
            now_ms(),
        )
        .unwrap();
    fixture
        .automation
        .record_pairing_canonical_url(
            &fixture.task,
            issued.generation,
            "https://chatgpt.com/c/owner-generation",
            now_ms(),
        )
        .unwrap();
    let wrong_generation = context(
        fixture._root.path(),
        Some(fixture.task.clone()),
        Some(fixture.task.clone()),
        Some(4),
        Some(fixture.automation.clone()),
    );

    let result = AutomationVerifyPairing
        .call_with_context(
            json!({"reference":issued.reference}),
            &fixture.config,
            &fixture.session,
            &wrong_generation,
        )
        .await;

    assert!(result.is_error);
    let pairing = fixture.automation.load(&fixture.task).unwrap().pairing;
    assert!(pairing.pending.is_none());
    assert!(pairing.verified_owner_generation.is_none());
}

#[tokio::test]
async fn active_goal_adds_browser_specific_guidance_to_the_agent_brief() {
    let fixture = fixture("brief-goal");
    let configured = fixture
        .automation
        .set_goal(&fixture.task, "Complete the active revision", 1000)
        .unwrap();

    let inactive = GetAgentBrief
        .call_with_context(
            json!({}),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    assert!(!inactive.joined_text().contains("## Active agent-chat goal"));

    fixture
        .automation
        .run_goal(&fixture.task, configured.revision, 1001)
        .unwrap();
    let active = GetAgentBrief
        .call_with_context(
            json!({}),
            &fixture.config,
            &fixture.session,
            &fixture.context,
        )
        .await;
    let text = active.joined_text();
    assert!(text.contains("## Active agent-chat goal"));
    assert!(text.contains("goal_read"));
    assert!(text.contains("goal_finish"));
    assert!(text.contains("goal_set"));
    assert!(text.contains("goal_resume"));
    assert!(text.contains(codexify::instructions::COMMAND_REJECTION_INSTRUCTIONS));
    assert!(text.contains(codexify::instructions::TURN_HANDOFF_INSTRUCTIONS));
    assert!(text.contains("a turn handoff is not goal completion"));
    assert!(text.contains("exact current revision"));
    assert!(text.contains("concrete reason"));
    assert!(text.contains("Do not request a separate turn"));
    assert!(!text.contains("Complete the active revision"));
}

#[tokio::test]
async fn completed_goal_releases_one_private_message_and_blocked_goal_releases_none() {
    let f = fixture("private-queue-goal-finish");
    let chat = f
        .context
        .markdown_chat
        .chat(&f.config, Some(&f.task), &f.session)
        .unwrap();
    chat.ensure().await.unwrap();
    chat.record_agent_call(now_ms()).await.unwrap();
    let goal = f
        .automation
        .set_goal(&f.task, "Finish this goal first", now_ms())
        .unwrap();
    f.automation
        .run_goal(&f.task, goal.revision, now_ms())
        .unwrap();
    for id in ["first", "second"] {
        chat.queue_user_message(id.into(), format!("private {id}"), vec![])
            .await
            .unwrap();
    }
    let blocked = GoalTool::Finish
        .call_with_context(
            json!({"expectedRevision":goal.revision,"status":"blocked","reason":"Need input","nextSteps":"Provide the missing input"}),
            &f.config,
            &f.session,
            &f.context,
        )
        .await;
    assert!(!blocked.is_error, "{}", blocked.joined_text());
    assert!(chat.read(true).await.unwrap().text.is_empty());
    assert_eq!(chat.queued_messages().await.unwrap().len(), 2);
    f.automation
        .run_goal(&f.task, goal.revision, now_ms())
        .unwrap();
    let finished = GoalTool::Finish
        .call_with_context(
            json!({"expectedRevision":goal.revision,"status":"completed","summary":"Done","nextSteps":"None"}),
            &f.config,
            &f.session,
            &f.context,
        )
        .await;
    assert!(!finished.is_error, "{}", finished.joined_text());
    assert_eq!(payload(&finished)["terminal"], false);
    assert_eq!(payload(&finished)["required_next_action"], "chat_read");
    assert_eq!(payload(&finished)["assistant_turn_may_end"], false);
    assert!(
        finished
            .new_chat_message_from_user
            .as_ref()
            .unwrap()
            .contains("private first")
    );
    assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    assert!(
        chat.read(true)
            .await
            .unwrap()
            .text
            .contains("private first")
    );
    assert!(!finished.joined_text().contains("private second"));
}

#[tokio::test]
async fn blocker_audit_uses_concrete_reason_when_report_next_steps_change() {
    let mut f = fixture("goal-report-blocker-audit");
    f.config.markdown_chat.automation.retry_timings.infinite_retries = true;
    f.automation = Arc::new(AgentAutomationStore::for_config(&f.config).unwrap());
    f.context.agent_automation = Some(f.automation.clone());
    let goal = f.automation.set_goal(&f.task, "Verify on the device", now_ms()).unwrap();
    f.automation.run_goal(&f.task, goal.revision, now_ms()).unwrap();
    for next in ["Connect the console", "Connect the console and run the prepared check"] {
        let result = GoalTool::Finish.call_with_context(
            json!({"expectedRevision":goal.revision,"status":"blocked","reason":"The physical console is unavailable","nextSteps":next}),
            &f.config, &f.session, &f.context,
        ).await;
        assert!(result.is_error, "A same-turn report must not block the infinite goal");
        let state = f.automation.load(&f.task).unwrap();
        let audit = state.blocker_audit.unwrap();
        assert_eq!(audit.reason, "The physical console is unavailable");
        assert_eq!(audit.attempts, 1);
        assert_eq!(state.goal.unwrap().status, GoalStatus::Active);
    }
}
