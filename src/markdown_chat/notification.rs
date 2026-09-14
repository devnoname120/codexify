use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{MarkdownChatConfig, NotificationState, NotificationsConfig};

pub async fn publish_config(
    config: &MarkdownChatConfig,
    workspace: &str,
    markdown: String,
    cancellation: &CancellationToken,
) -> NotificationState {
    if config.validate().is_err() {
        return NotificationState::Failed;
    }
    if let Some(notifications) = &config.notifications {
        publish_apprise(notifications, workspace, markdown, cancellation).await
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
