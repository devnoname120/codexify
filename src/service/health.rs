//! Windows service liveness, independent of agent inactivity and tunnel health.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use super::{RotatingLog, append_event, probe_service_health};

#[derive(Clone, Copy)]
pub(super) struct Policy {
    interval: Duration,
    request_timeout: Duration,
    failures: usize,
}

#[cfg(windows)]
pub(super) const POLICY: Policy = Policy {
    interval: Duration::from_secs(5),
    request_timeout: Duration::from_secs(3),
    failures: 6,
};

fn configured_port(config: &Path) -> Result<Option<u16>, String> {
    #[derive(Deserialize)]
    struct HttpConfig {
        port: Option<u16>,
    }
    let data = std::fs::read(config).map_err(|error| error.to_string())?;
    let config: HttpConfig = serde_json::from_slice(&data).map_err(|error| error.to_string())?;
    // Port zero is dynamically assigned and cannot be probed from this config.
    Ok(Some(config.port.unwrap_or(3000)).filter(|port| *port != 0))
}

pub(super) async fn watch(config: &Path, log: Arc<Mutex<RotatingLog>>, policy: Policy) -> String {
    let port = match configured_port(config) {
        Ok(Some(port)) => port,
        result => {
            append_event(
                &log,
                &format!("server health monitoring unavailable: {result:?}"),
            );
            return std::future::pending().await;
        }
    };
    let client = match crate::tls::client_builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(policy.request_timeout)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            append_event(&log, &format!("server health client unavailable: {error}"));
            return std::future::pending().await;
        }
    };
    let url = format!("http://127.0.0.1:{port}/health");
    let mut armed = false;
    let mut failed = 0;
    loop {
        match probe_service_health(&client, &url).await {
            Ok(()) => {
                if !armed {
                    append_event(&log, &format!("Codexify health monitoring armed at {url}"));
                } else if failed > 0 {
                    append_event(
                        &log,
                        &format!("Codexify health recovered after {failed} failed probes"),
                    );
                }
                armed = true;
                failed = 0;
            }
            Err(error) if armed => {
                failed += 1;
                append_event(
                    &log,
                    &format!(
                        "Codexify health probe failed ({failed}/{}): {error}",
                        policy.failures
                    ),
                );
                if failed >= policy.failures {
                    return format!(
                        "local HTTP health failed {failed} consecutive checks: {error}"
                    );
                }
            }
            Err(_) => {} // A slow first startup must not become a restart loop.
        }
        tokio::time::sleep(policy.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn fast_policy() -> Policy {
        Policy {
            interval: Duration::from_millis(5),
            request_timeout: Duration::from_millis(500),
            failures: 3,
        }
    }

    #[test]
    fn ports_match_service_configuration_and_dynamic_ports_are_not_guessed() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        for (data, expected) in [
            ("{}", Some(3000)),
            (r#"{"port":null}"#, Some(3000)),
            (r#"{"port":43179}"#, Some(43179)),
            (r#"{"port":0}"#, None),
        ] {
            std::fs::write(&config, data).unwrap();
            assert_eq!(configured_port(&config).unwrap(), expected);
        }
        std::fs::write(&config, r#"{"port":-1}"#).unwrap();
        assert!(configured_port(&config).is_err());
    }

    #[tokio::test]
    async fn startup_is_unbounded_until_first_healthy_response_and_tunnel_readiness_is_independent()
    {
        let healthy = Arc::new(AtomicBool::new(false));
        let value = healthy.clone();
        let app = axum::Router::new().route("/health", axum::routing::get(move || {
            let value = value.clone();
            async move { axum::Json(serde_json::json!({"status":if value.load(Ordering::SeqCst) {"ok"} else {"starting"},"ready":false})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        let log_path = root.path().join("log");
        std::fs::write(&config, format!(r#"{{"port":{port}}}"#)).unwrap();
        let log = Arc::new(Mutex::new(
            RotatingLog::open(log_path.clone(), 1024 * 1024, 1).unwrap(),
        ));
        let watch = watch(&config, log, fast_policy());
        tokio::pin!(watch);
        assert!(
            tokio::time::timeout(Duration::from_millis(80), &mut watch)
                .await
                .is_err()
        );
        healthy.store(true, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut watch)
                    .await
                    .is_err()
            );
            if std::fs::read_to_string(&log_path)
                .unwrap()
                .contains("health monitoring armed")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "health monitoring did not arm"
            );
        }
        healthy.store(false, Ordering::SeqCst);
        let reason = tokio::time::timeout(Duration::from_secs(2), &mut watch)
            .await
            .unwrap();
        assert!(reason.contains("3 consecutive checks"));
        server.abort();
    }

    #[tokio::test]
    async fn transient_failures_reset_and_hung_requests_are_bounded() {
        let requests = Arc::new(AtomicUsize::new(0));
        let value = requests.clone();
        let begin = Arc::new(AtomicBool::new(false));
        let begin_handler = begin.clone();
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move || {
                let value = value.clone();
                let begin = begin_handler.clone();
                async move {
                    if !begin.load(Ordering::SeqCst) {
                        return axum::Json(serde_json::json!({"status":"ok"}));
                    }
                    let request = value.fetch_add(1, Ordering::SeqCst);
                    if request >= 3 {
                        std::future::pending::<()>().await;
                    }
                    axum::Json(serde_json::json!({"status":if request<2 {"starting"} else {"ok"}}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        let log_path = root.path().join("log");
        std::fs::write(&config, format!(r#"{{"port":{port}}}"#)).unwrap();
        let log = Arc::new(Mutex::new(
            RotatingLog::open(log_path.clone(), 1024 * 1024, 1).unwrap(),
        ));
        let watch = watch(&config, log, fast_policy());
        tokio::pin!(watch);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut watch)
                    .await
                    .is_err()
            );
            if std::fs::read_to_string(&log_path)
                .unwrap()
                .contains("health monitoring armed")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "health monitoring did not arm"
            );
        }
        begin.store(true, Ordering::SeqCst);
        let reason = tokio::time::timeout(Duration::from_secs(5), &mut watch)
            .await
            .unwrap();
        assert!(reason.contains("3 consecutive checks"));
        assert!(requests.load(Ordering::SeqCst) >= 6);
        assert!(
            std::fs::read_to_string(log_path)
                .unwrap()
                .contains("recovered after 2 failed probes")
        );
        server.abort();
    }
}
