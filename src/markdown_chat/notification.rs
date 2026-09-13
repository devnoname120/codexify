use std::time::Duration;

use base64::Engine;
use tokio_util::sync::CancellationToken;

use super::{NotificationState, NtfyConfig};

pub async fn publish(
    config: &NtfyConfig,
    workspace: &str,
    markdown: String,
    cancellation: &CancellationToken,
) -> NotificationState {
    if cancellation.is_cancelled() {
        return NotificationState::Cancelled;
    }
    let Ok(client) = crate::tls::client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return NotificationState::Failed;
    };
    let title = format!("Codexify - {workspace}");
    let title = format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(title)
    );
    let mut request = client
        .post(&config.url)
        .header("Content-Type", "text/markdown; charset=utf-8")
        .header("Markdown", "yes")
        .header("Title", title)
        .body(markdown);
    if let Some(token) = &config.token {
        request = request.bearer_auth(token);
    }
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => NotificationState::Cancelled,
        response = request.send() => {
            if response.is_ok_and(|response| response.status().is_success()) {
                NotificationState::Accepted
            } else {
                NotificationState::Failed
            }
        }
    }
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
            "The ntfy server accepted the notification. This does not confirm that the user has seen it."
        }
        NotificationState::Failed => {
            "The message is saved in CHAT.md, but the notification could not be delivered."
        }
        NotificationState::Cancelled => {
            "The message is saved in CHAT.md, but notification delivery was cancelled."
        }
    }
}
