use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{MarkdownChatConfig, NotificationState, NotificationsConfig};

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationEvent {
    #[default]
    Message,
    Completed,
    InputRequired,
    GoalBlocked,
    Error,
    Offline,
}

impl NotificationsConfig {
    pub fn includes(&self, event: NotificationEvent) -> bool {
        match event {
            NotificationEvent::Message => self.events.messages,
            NotificationEvent::Completed => self.events.completed,
            NotificationEvent::InputRequired => self.events.input_required,
            NotificationEvent::GoalBlocked => self.events.goal_blocked,
            NotificationEvent::Error => self.events.errors,
            NotificationEvent::Offline => self.events.offline,
        }
    }
}

pub fn enabled(config: &MarkdownChatConfig, event: NotificationEvent) -> bool {
    config
        .notifications
        .as_ref()
        .is_some_and(|notifications| notifications.includes(event))
}

pub(crate) async fn enabled_for_chat(
    config: &MarkdownChatConfig,
    event: NotificationEvent,
    chat: &std::sync::Arc<super::ChatFile>,
) -> bool {
    if !enabled(config, event) {
        return false;
    }
    if config.notifications.as_ref().is_some_and(|n| n.subagents) {
        return true;
    }
    let path = chat.path().to_path_buf();
    match tokio::task::spawn_blocking(move || {
        crate::agent_automation::AgentAutomationStore::chat_is_subagent(&path)
    })
    .await
    {
        Ok(Ok(worker)) => !worker,
        _ => {
            tracing::warn!("notification suppressed because agent membership could not be read");
            false
        }
    }
}

pub(crate) async fn notify_input(
    chat: &std::sync::Arc<super::ChatFile>,
    config: &MarkdownChatConfig,
    workspace: &str,
) -> Result<(), String> {
    if !enabled_for_chat(config, NotificationEvent::InputRequired, chat).await
        || !chat.claim_input_notification().await?
    {
        return Ok(());
    }
    let config = config
        .notifications
        .as_ref()
        .expect("enabled notification")
        .clone();
    let workspace = chat
        .owner_summary()
        .await
        .map(|summary| summary.title)
        .unwrap_or_else(|_| workspace.to_owned());
    tokio::spawn(async move {
        let outcome = publish_apprise(
            &config,
            &workspace,
            "The chat is waiting for your input. Open the chat to read the agent's question."
                .into(),
            &CancellationToken::new(),
        )
        .await;
        tracing::info!(?outcome, "chat input notification attempted");
    });
    Ok(())
}

pub(crate) async fn notify_automation(
    chat: &std::sync::Arc<super::ChatFile>,
    config: &NotificationsConfig,
    workspace: &str,
    state: &crate::agent_automation::AutomationTaskState,
) -> Result<(), String> {
    use crate::agent_automation::{AutomationActionStatus as Action, GoalStatus};
    if !config.subagents && state.agents.parent_task_key.is_some() {
        return Ok(());
    }
    if let Some(goal) = &state.goal {
        let event = match goal.status {
            GoalStatus::Completed => Some((
                NotificationEvent::Completed,
                "Goal completed",
                goal.summary.as_deref().unwrap_or(&goal.markdown),
            )),
            GoalStatus::Blocked => Some((
                NotificationEvent::GoalBlocked,
                "Goal blocked",
                goal.blocked_reason
                    .as_deref()
                    .unwrap_or("The goal needs your input."),
            )),
            _ => None,
        };
        if let Some((event, caption, detail)) = event {
            let key = format!(
                "{}:{:?}:{}",
                goal.revision,
                goal.status,
                goal.finished_at_ms.unwrap_or(goal.updated_at_ms)
            );
            if config.includes(event) && chat.claim_notification("goal", &key).await? {
                let label = chat
                    .owner_summary()
                    .await
                    .map(|summary| summary.title)
                    .unwrap_or_else(|_| workspace.to_owned());
                dispatch(
                    config,
                    &label,
                    format!(
                        "{caption}.\n\n{}",
                        detail.chars().take(2000).collect::<String>()
                    ),
                );
            }
        }
    }
    let action = &state.action;
    let needs_attention = matches!(
        action.status,
        Action::Failed
            | Action::Interference
            | Action::Ambiguous
            | Action::Stalled
            | Action::AuthenticationRequired
            | Action::RateLimited
            | Action::ApprovalRequired
    ) || (action.status == Action::Unverified && action.last_error.is_some());
    if !needs_attention {
        if action.status == Action::Idle {
            chat.clear_notification_claim("error").await?;
        }
        return Ok(());
    }
    // Notify only after automatic retries are exhausted, not on each attempt.
    if state.automatic_retry_at_ms().is_some() {
        return Ok(());
    }
    let event = if action.status == Action::ApprovalRequired {
        NotificationEvent::InputRequired
    } else {
        NotificationEvent::Error
    };
    let detail = action
        .last_error
        .as_deref()
        .unwrap_or("Browser automation could not continue. Open the chat for details and retry.");
    // Identical failures stay one episode, including retries that receive new action IDs.
    use sha2::{Digest, Sha256};
    let key = format!(
        "{:x}",
        Sha256::digest(format!(
            "{}:{:?}:{:?}:{}",
            state.pairing.generation, action.kind, action.status, detail
        ))
    );
    if config.includes(event) && chat.claim_notification("error", &key).await? {
        let caption = if event == NotificationEvent::InputRequired {
            "Chat needs your approval"
        } else {
            "Chat could not continue"
        };
        let label = chat
            .owner_summary()
            .await
            .map(|summary| summary.title)
            .unwrap_or_else(|_| workspace.to_owned());
        dispatch(
            config,
            &label,
            format!(
                "{caption}.\n\n{}",
                detail.chars().take(2000).collect::<String>()
            ),
        );
    }
    Ok(())
}

fn dispatch(config: &NotificationsConfig, workspace: &str, body: String) {
    let config = config.clone();
    let workspace = workspace.to_owned();
    tokio::spawn(async move {
        let outcome = publish_apprise(&config, &workspace, body, &CancellationToken::new()).await;
        tracing::info!(?outcome, "chat state notification attempted");
    });
}

pub async fn publish_config(
    config: &MarkdownChatConfig,
    workspace: &str,
    markdown: String,
    cancellation: &CancellationToken,
) -> NotificationState {
    publish_event(
        config,
        NotificationEvent::Message,
        workspace,
        markdown,
        cancellation,
    )
    .await
}

pub async fn publish_event(
    config: &MarkdownChatConfig,
    event: NotificationEvent,
    workspace: &str,
    markdown: String,
    cancellation: &CancellationToken,
) -> NotificationState {
    if config.validate().is_err() {
        return NotificationState::Failed;
    }
    if let Some(notifications) = &config.notifications {
        if notifications.includes(event) {
            publish_apprise(notifications, workspace, markdown, cancellation).await
        } else {
            NotificationState::Suppressed
        }
    } else {
        NotificationState::NotConfigured
    }
}

async fn publish_apprise(
    config: &NotificationsConfig,
    workspace: &str,
    markdown: String,
    cancellation: &CancellationToken,
) -> NotificationState {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    if cancellation.is_cancelled() {
        return NotificationState::Cancelled;
    }
    let payload = serde_json::to_vec(&serde_json::json!({
        "urls": config.urls, "title": format!("Codexify - {workspace}"), "body": markdown
    }))
    .expect("notification payload");
    let mut command = tokio::process::Command::new(&config.python_path);
    command
        .args(["-I", "-c", include_str!("apprise_notify.py")])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let Ok(mut child) = command.spawn() else {
        return NotificationState::Failed;
    };
    let mut input = child.stdin.take().expect("piped notification input");
    let outcome = {
        let work = async {
            input.write_all(&payload).await?;
            drop(input);
            child.wait().await
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => NotificationState::Cancelled,
            result = tokio::time::timeout(Duration::from_millis(config.timeout_ms), work) => {
                if matches!(result, Ok(Ok(status)) if status.success()) {
                    NotificationState::Accepted
                } else { NotificationState::Failed }
            }
        }
    };
    if child.id().is_some() {
        let _ = child.kill().await;
    }
    outcome
}

pub fn description(state: NotificationState) -> &'static str {
    match state {
        NotificationState::NotConfigured => {
            "The transcript is available in CHAT.md; no notification provider is configured."
        }
        NotificationState::Suppressed => {
            "The message is saved in CHAT.md; this notification category is disabled or has already been notified."
        }
        NotificationState::Pending => {
            "The message is saved in CHAT.md; notification delivery is not yet confirmed."
        }
        NotificationState::Accepted => {
            "The configured notification provider reported success. This does not confirm that the user has seen it."
        }
        NotificationState::Failed => {
            "The message is saved in CHAT.md, but notification delivery did not fully succeed. Check the service URLs, Python interpreter, and Apprise installation. Do not resend the chat message merely to retry notifications."
        }
        NotificationState::Cancelled => {
            "The message is saved in CHAT.md, but notification delivery was cancelled."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_automation::{
        AutomationActionKind, AutomationActionStatus, AutomationTaskState, GoalStatus,
    };
    use std::sync::Arc;

    fn config() -> MarkdownChatConfig {
        serde_json::from_value(serde_json::json!({"notifications":{
            "urls":["ntfys://test"],"pythonPath":"missing-notification-interpreter",
            "events":{"messages":false,"offline":false}
        }}))
        .unwrap()
    }

    fn claims(root: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(root.join("cursor.json")).unwrap()).unwrap()
    }

    fn membership(root: &std::path::Path, parent: serde_json::Value) {
        let path = root.join("automation.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "agents":{"parentTaskKey":parent}, "unrelatedFutureField":true
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[tokio::test]
    async fn subagent_scope_filters_every_event_and_preserves_root_event_filters() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(super::super::ChatFile::new(
            root.path().join("CHAT.md"),
            true,
        ));
        let mut cfg: MarkdownChatConfig = serde_json::from_value(serde_json::json!({
            "notifications":{"urls":["ntfys://test"],"subagents":false}
        }))
        .unwrap();
        for event in [
            NotificationEvent::Message,
            NotificationEvent::Completed,
            NotificationEvent::InputRequired,
            NotificationEvent::GoalBlocked,
            NotificationEvent::Error,
            NotificationEvent::Offline,
        ] {
            assert!(
                enabled_for_chat(&cfg, event, &chat).await,
                "ordinary chat: {event:?}"
            );
            membership(root.path(), serde_json::json!("a".repeat(64)));
            assert!(
                !enabled_for_chat(&cfg, event, &chat).await,
                "worker: {event:?}"
            );
            cfg.notifications.as_mut().unwrap().subagents = true;
            assert!(
                enabled_for_chat(&cfg, event, &chat).await,
                "worker opted in: {event:?}"
            );
            cfg.notifications.as_mut().unwrap().subagents = false;
            membership(root.path(), serde_json::Value::Null);
            assert!(
                enabled_for_chat(&cfg, event, &chat).await,
                "coordinator: {event:?}"
            );
        }
        cfg.notifications.as_mut().unwrap().events.completed = false;
        assert!(!enabled_for_chat(&cfg, NotificationEvent::Completed, &chat).await);
        membership(root.path(), serde_json::json!("a".repeat(64)));
        cfg.notifications.as_mut().unwrap().subagents = true;
        assert!(!enabled_for_chat(&cfg, NotificationEvent::Completed, &chat).await);
    }

    #[tokio::test]
    async fn unreadable_membership_suppresses_delivery_without_failing_chat() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(super::super::ChatFile::new(
            root.path().join("CHAT.md"),
            true,
        ));
        let mut cfg = config();
        cfg.notifications.as_mut().unwrap().subagents = false;
        membership(root.path(), serde_json::json!("invalid-parent"));
        assert!(!enabled_for_chat(&cfg, NotificationEvent::InputRequired, &chat).await);
        std::fs::write(root.path().join("automation.json"), b"{").unwrap();
        assert!(!enabled_for_chat(&cfg, NotificationEvent::InputRequired, &chat).await);
        chat.append("Which option?".into()).await.unwrap();
        notify_input(&chat, &cfg, "test").await.unwrap();
        assert!(chat.claim_input_notification().await.unwrap());
    }

    #[tokio::test]
    async fn muted_worker_alerts_do_not_consume_input_goal_or_error_claims() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(super::super::ChatFile::new(
            root.path().join("CHAT.md"),
            true,
        ));
        membership(root.path(), serde_json::json!("a".repeat(64)));
        let mut cfg = config();
        cfg.notifications.as_mut().unwrap().subagents = false;
        chat.append("Which option?".into()).await.unwrap();
        notify_input(&chat, &cfg, "test").await.unwrap();
        assert!(chat.claim_input_notification().await.unwrap());
        let mut state = AutomationTaskState::default();
        state.agents.parent_task_key = Some("a".repeat(64));
        state.goal = Some(
            serde_json::from_value(serde_json::json!({
                "revision":1,"markdown":"Finish task","status":"blocked","createdAtMs":1,
                "updatedAtMs":2,"finishedAtMs":2,"summary":null,"blockedReason":"Need an answer"
            }))
            .unwrap(),
        );
        state.action.id = Some("action-1".into());
        state.action.kind = Some(AutomationActionKind::OfflineChat);
        state.action.last_error = Some("Chat cannot be opened".into());
        state.action.automatic_retry_attempts = 5;
        for action in [
            AutomationActionStatus::Failed,
            AutomationActionStatus::ApprovalRequired,
        ] {
            state.action.status = action;
            for goal in [GoalStatus::Blocked, GoalStatus::Completed] {
                state.goal.as_mut().unwrap().status = goal;
                notify_automation(&chat, cfg.notifications.as_ref().unwrap(), "test", &state)
                    .await
                    .unwrap();
                assert!(
                    claims(root.path())["notificationClaims"]
                        .get("goal")
                        .is_none()
                );
                assert!(
                    claims(root.path())["notificationClaims"]
                        .get("error")
                        .is_none()
                );
            }
        }
        // The same settings still deliver coordinator transitions.
        state.agents.parent_task_key = None;
        notify_automation(&chat, cfg.notifications.as_ref().unwrap(), "test", &state)
            .await
            .unwrap();
        assert!(claims(root.path())["notificationClaims"]["goal"].is_string());
        assert!(claims(root.path())["notificationClaims"]["error"].is_string());
    }

    #[tokio::test]
    async fn input_alert_survives_repeated_waits_progress_and_restart_until_user_replies() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("CHAT.md");
        let chat = Arc::new(super::super::ChatFile::new(path.clone(), true));
        assert!(!chat.claim_input_notification().await.unwrap());
        chat.append("Which option should I use?".into())
            .await
            .unwrap();
        let (first, second) = tokio::join!(
            chat.claim_input_notification(),
            chat.claim_input_notification()
        );
        assert_eq!(
            [first.unwrap(), second.unwrap()]
                .into_iter()
                .filter(|value| *value)
                .count(),
            1
        );
        for call in 1..5 {
            chat.record_agent_call(call).await.unwrap();
            notify_input(&chat, &config(), "test").await.unwrap();
            assert!(!chat.claim_input_notification().await.unwrap());
        }
        chat.append("Still waiting on that same question.".into())
            .await
            .unwrap();
        assert!(!chat.claim_input_notification().await.unwrap());
        let reopened = Arc::new(super::super::ChatFile::new(path, true));
        assert!(!reopened.claim_input_notification().await.unwrap());
        reopened
            .append_user("reply-1".into(), "Use option A.".into())
            .await
            .unwrap();
        assert!(
            !reopened.claim_input_notification().await.unwrap(),
            "unread input must not trigger another alert"
        );
        reopened.read(true).await.unwrap();
        reopened
            .append("What should I do next?".into())
            .await
            .unwrap();
        assert!(reopened.claim_input_notification().await.unwrap());
    }

    #[tokio::test]
    async fn completion_and_stop_do_not_trigger_idle_input_alerts() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(super::super::ChatFile::new(
            root.path().join("CHAT.md"),
            true,
        ));
        chat.append_with_event("Done.".into(), NotificationEvent::Completed)
            .await
            .unwrap();
        assert!(!chat.claim_input_notification().await.unwrap());
        chat.append("Which next task?".into()).await.unwrap();
        assert!(chat.claim_input_notification().await.unwrap());
        chat.append_user("reply".into(), "Continue.".into())
            .await
            .unwrap();
        chat.read(true).await.unwrap();
        chat.request_agent_stop("stop".into()).await.unwrap();
        assert!(!chat.claim_input_notification().await.unwrap());
    }

    #[tokio::test]
    async fn goal_alerts_are_persisted_once_per_terminal_transition() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("CHAT.md");
        let chat = Arc::new(super::super::ChatFile::new(path.clone(), true));
        let cfg = config();
        let cfg = cfg.notifications.unwrap();
        let mut state = AutomationTaskState::default();
        state.goal = Some(serde_json::from_value(serde_json::json!({
            "revision":1,"markdown":"Finish task","status":"blocked","createdAtMs":1,"updatedAtMs":2,
            "finishedAtMs":2,"summary":null,"blockedReason":"Need an answer"
        })).unwrap());
        notify_automation(&chat, &cfg, "test", &state)
            .await
            .unwrap();
        let key = claims(root.path())["notificationClaims"]["goal"].clone();
        let reopened = Arc::new(super::super::ChatFile::new(path, true));
        notify_automation(&reopened, &cfg, "test", &state)
            .await
            .unwrap();
        assert_eq!(claims(root.path())["notificationClaims"]["goal"], key);
        let goal = state.goal.as_mut().unwrap();
        goal.status = GoalStatus::Active;
        goal.finished_at_ms = None;
        notify_automation(&reopened, &cfg, "test", &state)
            .await
            .unwrap();
        assert_eq!(claims(root.path())["notificationClaims"]["goal"], key);
        let goal = state.goal.as_mut().unwrap();
        goal.status = GoalStatus::Blocked;
        goal.finished_at_ms = Some(3);
        notify_automation(&reopened, &cfg, "test", &state)
            .await
            .unwrap();
        assert_ne!(claims(root.path())["notificationClaims"]["goal"], key);
        let goal = state.goal.as_mut().unwrap();
        goal.status = GoalStatus::Completed;
        goal.finished_at_ms = Some(4);
        notify_automation(&reopened, &cfg, "test", &state)
            .await
            .unwrap();
        assert!(
            claims(root.path())["notificationClaims"]["goal"]
                .as_str()
                .unwrap()
                .contains("Completed")
        );
    }

    #[tokio::test]
    async fn retrying_failures_stay_quiet_and_identical_terminal_errors_are_one_episode() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("CHAT.md");
        let chat = Arc::new(super::super::ChatFile::new(path.clone(), true));
        let cfg = config().notifications.unwrap();
        chat.ensure().await.unwrap();
        let mut state = AutomationTaskState::default();
        state.action.id = Some("action-1".into());
        state.action.kind = Some(AutomationActionKind::OfflineChat);
        state.action.status = AutomationActionStatus::Failed;
        state.action.send_accepted = Some(false);
        state.action.updated_at_ms = Some(1);
        state.action.last_error =
            Some("ChatGPT did not stop the previous response; the new message was not sent".into());
        assert!(state.automatic_retry_at_ms().is_some());
        notify_automation(&chat, &cfg, "test", &state)
            .await
            .unwrap();
        assert!(
            claims(root.path())["notificationClaims"]
                .get("error")
                .is_none()
        );
        state.action.last_error = Some("Chat cannot be opened".into());
        state.action.automatic_retry_attempts = 5;
        notify_automation(&chat, &cfg, "test", &state)
            .await
            .unwrap();
        let key = claims(root.path())["notificationClaims"]["error"].clone();
        let reopened = Arc::new(super::super::ChatFile::new(path, true));
        state.action.id = Some("retry-action".into());
        state.action.updated_at_ms = Some(60_000);
        notify_automation(&reopened, &cfg, "test", &state)
            .await
            .unwrap();
        assert_eq!(claims(root.path())["notificationClaims"]["error"], key);
        state.action.status = AutomationActionStatus::Idle;
        notify_automation(&reopened, &cfg, "test", &state)
            .await
            .unwrap();
        assert!(
            claims(root.path())["notificationClaims"]
                .get("error")
                .is_none()
        );
        state.action.status = AutomationActionStatus::Failed;
        notify_automation(&reopened, &cfg, "test", &state)
            .await
            .unwrap();
        assert_eq!(claims(root.path())["notificationClaims"]["error"], key);
    }
}
