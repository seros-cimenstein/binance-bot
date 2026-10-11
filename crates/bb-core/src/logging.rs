//! `tracing` setup shared by every binary entry point.

use tracing_subscriber::EnvFilter;

/// Initialise stderr logging. `RUST_LOG` wins; otherwise `LOG_LEVEL`
/// (`DEBUG`/`INFO`/...) or `info`. `LOG_FORMAT=json` switches to JSON lines.
/// Safe to call more than once (later calls are no-ops).
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = crate::env::string(&["LOG_LEVEL"], "info").to_ascii_lowercase();
        EnvFilter::new(format!(
            "{level},hyper=warn,reqwest=warn,rustls=warn,tungstenite=warn"
        ))
    });
    let json = crate::env::string(&["LOG_FORMAT"], "text").eq_ignore_ascii_case("json");
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false);
    // try_init fails only when a subscriber is already installed.
    let _ = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
}
