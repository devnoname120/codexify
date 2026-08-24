use tracing::{Level, Metadata};
use tracing_subscriber::filter::{FilterExt, filter_fn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub fn default_filter(verbosity: u8) -> &'static str {
    match verbosity {
        0 => "info",
        1 => "codexify=debug,rmcp=warn",
        _ => "codexify=trace,rmcp=warn",
    }
}

pub fn init(verbosity: u8) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_filter(verbosity)));
    // Suppress `rmcp` protocol-internal events regardless of the resolved
    // filter so operators cannot force transport payloads into the logs via
    // `RUST_LOG` (they can carry model/user content).
    let payload_guard = filter_fn(framework_metadata_allowed);
    let layer = tracing_subscriber::fmt::layer().with_filter(env_filter.and(payload_guard));

    tracing_subscriber::registry().with(layer).init();
}

fn framework_metadata_allowed(metadata: &Metadata<'_>) -> bool {
    framework_event_allowed(metadata.target(), metadata.level())
}

pub fn framework_event_allowed(target: &str, _level: &Level) -> bool {
    !target.starts_with("rmcp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_targets_codexify_before_protocol_internals() {
        assert_eq!(default_filter(0), "info");
        assert_eq!(default_filter(1), "codexify=debug,rmcp=warn");
        assert_eq!(default_filter(2), "codexify=trace,rmcp=warn");
        assert_eq!(default_filter(u8::MAX), "codexify=trace,rmcp=warn");
    }

    #[test]
    fn rmcp_protocol_events_cannot_be_enabled_by_rust_log() {
        assert!(!framework_event_allowed("rmcp::service", &Level::ERROR));
        assert!(!framework_event_allowed("rmcp::service", &Level::WARN));
        assert!(!framework_event_allowed("rmcp::service", &Level::INFO));
        assert!(!framework_event_allowed("rmcp::service", &Level::DEBUG));
        assert!(!framework_event_allowed("rmcp::transport", &Level::TRACE));
        assert!(framework_event_allowed("codexify::server", &Level::DEBUG));
    }
}
