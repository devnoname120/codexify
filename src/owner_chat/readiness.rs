use super::*;

pub(super) fn describe(health: &Value, browser: Option<&Value>, refresh: Option<&Value>) -> Value {
    let status = |ready, loading, browser_only, phase, detail| json!({"ready":ready,"loading":loading,"browserOnly":browser_only,"phase":phase,"detail":detail});
    if health["status"] != "ok" {
        return if health.get("tunnelsConfigured").is_some() {
            status(
                false,
                true,
                false,
                "starting_tunnels",
                "Connecting ChatGPT tunnels…",
            )
        } else {
            status(
                false,
                true,
                false,
                "starting_server",
                "Starting Codexify and loading MCP tools…",
            )
        };
    }
    if refresh.is_some_and(|value| value["phase"] == "failed") {
        return status(
            false,
            false,
            true,
            "connector_failed",
            "Connector refresh needs attention. Open settings to retry.",
        );
    }
    if let Some(browser) = browser {
        if browser["manuallyStopped"] == true {
            return status(
                false,
                false,
                true,
                "browser_stopped",
                "The browser companion is stopped.",
            );
        }
        if browser["status"] == "authentication_required" {
            return status(
                false,
                false,
                true,
                "sign_in_required",
                "Sign in to ChatGPT in Browser settings.",
            );
        }
        if browser["authenticated"] != true
            || browser["status"] == "unavailable"
            || browser["recovering"] == true
            || browser["stopping"] == true
        {
            return status(
                false,
                true,
                true,
                "starting_browser",
                "Starting the browser companion…",
            );
        }
    }
    if let Some(refresh) = refresh {
        let restarted = refresh["updateId"].is_string()
            && refresh["targetVersion"] == env!("CARGO_PKG_VERSION")
            || refresh["originPid"]
                .as_u64()
                .is_some_and(|pid| pid != u64::from(std::process::id()));
        if refresh["phase"] == "waiting_for_restart" && restarted {
            return if browser.is_some() {
                status(
                    false,
                    true,
                    false,
                    "waiting_for_connector",
                    "Waiting to refresh ChatGPT connector tools…",
                )
            } else {
                status(
                    true,
                    false,
                    false,
                    "manual_refresh_required",
                    "Codexify is ready. Refresh connector tools in ChatGPT, or enable browser automation.",
                )
            };
        }
        match refresh["phase"].as_str() {
            Some("running") => {
                return status(
                    false,
                    true,
                    false,
                    "refreshing_connector",
                    "Refreshing ChatGPT connector tools…",
                );
            }
            Some("queued" | "starting_browser") => {
                return status(
                    false,
                    true,
                    false,
                    "waiting_for_connector",
                    "Waiting to refresh ChatGPT connector tools…",
                );
            }
            _ => {}
        }
    }
    // A normal busy browser action, or an unapplied settings edit, does not make
    // an otherwise healthy app unavailable. Queued chat messages still work.
    status(true, false, false, "ready", "Codexify is ready.")
}

pub(super) async fn snapshot(state: &OwnerState, refresh: Option<&Value>) -> (Value, Value) {
    let mut health = async {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/health", state.config.port))
            .timeout(Duration::from_secs(2))
            .send()
            .await?
            .json::<Value>()
            .await
    }
    .await
    .unwrap_or_else(|_| json!({"status":"starting"}));
    if !health.is_object() {
        health = json!({"status":"starting"});
    }
    let browser = if state.config.markdown_chat.automation.enabled {
        Some(browser_snapshot(state).await.0)
    } else {
        None
    };
    let readiness = describe(&health, browser.as_ref(), refresh);
    (health, readiness)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_waits_for_startup_but_not_ordinary_browser_work_or_unapplied_edits() {
        let ok = json!({"status":"ok"});
        let busy = json!({"status":"busy","authenticated":true,"activeActionId":"ordinary-send"});
        assert_eq!(describe(&ok, Some(&busy), None)["ready"], true);
        assert_eq!(
            describe(
                &ok,
                Some(&busy),
                Some(&json!({"phase":"waiting_for_restart"}))
            )["ready"],
            true
        );
        assert_eq!(describe(&ok, None, None)["ready"], true);
        for (health, browser, refresh, phase, browser_only) in [
            (
                json!({"status":"starting"}),
                None,
                None,
                "starting_server",
                false,
            ),
            (
                json!({"status":"starting","tunnelsConfigured":2}),
                None,
                None,
                "starting_tunnels",
                false,
            ),
            (
                ok.clone(),
                Some(json!({"status":"unavailable","authenticated":false})),
                None,
                "starting_browser",
                true,
            ),
            (
                ok.clone(),
                Some(busy.clone()),
                Some(json!({"phase":"running"})),
                "refreshing_connector",
                false,
            ),
        ] {
            let value = describe(&health, browser.as_ref(), refresh.as_ref());
            assert_eq!(value["ready"], false);
            assert_eq!(value["loading"], true);
            assert_eq!(value["phase"], phase);
            assert_eq!(value["browserOnly"], browser_only);
        }
        for (browser, phase) in [
            (
                json!({"manuallyStopped":true,"status":"unavailable"}),
                "browser_stopped",
            ),
            (
                json!({"status":"authentication_required","authenticated":false}),
                "sign_in_required",
            ),
        ] {
            let value = describe(&ok, Some(&browser), None);
            assert_eq!(value["phase"], phase);
            assert_eq!(
                value["loading"], false,
                "needs user action, rather than an endless startup spinner"
            );
        }
        let failed = describe(
            &ok,
            Some(&json!({"status":"unavailable"})),
            Some(&json!({"phase":"failed"})),
        );
        assert_eq!(failed["phase"], "connector_failed");
        assert_eq!(failed["loading"], false);
    }
}
