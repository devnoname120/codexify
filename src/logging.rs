use tracing_subscriber::EnvFilter;

pub fn default_filter(verbosity: u8) -> &'static str {
    match verbosity {
        0 => "info",
        1 => "codexify=debug,rmcp=warn",
        _ => "codexify=trace,rmcp=warn",
    }
}

pub fn init(verbosity: u8) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_filter(verbosity)));
    tracing_subscriber::fmt().with_env_filter(filter).init();
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
}
