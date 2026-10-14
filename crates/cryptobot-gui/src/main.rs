//! `cryptobot-gui` — read-only localhost web dashboard for the bot fleet.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use bb_capital::status::{default_data_dir, wallet_client};
use clap::Parser;
use cryptobot_gui::{AppState, ClaudeSpotFiles, router};

#[derive(Parser, Debug)]
#[command(
    name = "cryptobot-gui",
    version,
    about = "Read-only localhost web dashboard: wallet, Claude Spot activity, fleet status, X insighter signals"
)]
struct Cli {
    /// Listen address. Requests must still address localhost in their Host header.
    #[arg(long, env = "CRYPTOBOT_GUI_BIND", default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    /// Fleet data dir read by the fleet and signals views (BOT_STATUS_DATA_DIR).
    #[arg(long, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    /// Open the dashboard in the default browser once it is listening. If another instance
    /// already holds the address, just open that one.
    #[arg(long)]
    open: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    bb_core::env::load_dotenv();
    bb_core::logging::init();
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = format!("{err:#}"), "cryptobot-gui failed");
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let listener = match tokio::net::TcpListener::bind(cli.bind).await {
        Ok(listener) => listener,
        Err(error) if cli.open && error.kind() == std::io::ErrorKind::AddrInUse => {
            let url = dashboard_url(cli.bind.port());
            eprintln!(
                "cryptobot-gui: already running on {}; opening {url}",
                cli.bind
            );
            open_browser(&url);
            return Ok(());
        }
        Err(error) => return Err(error).with_context(|| format!("cannot listen on {}", cli.bind)),
    };
    let client = wallet_client();
    if client.is_none() {
        tracing::warn!("no Binance credentials; the wallet tab stays empty");
    }
    let app = router(AppState::new(
        cli.data_dir,
        ClaudeSpotFiles::from_env(),
        client,
    ));
    let addr = listener.local_addr()?;
    let url = dashboard_url(addr.port());
    tracing::info!(%addr, "cryptobot-gui listening (read-only)");
    eprintln!("cryptobot-gui: {url}");
    if cli.open {
        open_browser(&url);
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")
}

/// The page must be addressed as `localhost` to pass the Host check.
fn dashboard_url(port: u16) -> String {
    format!("http://localhost:{port}/")
}

/// Hand `url` to the desktop's default browser; failures only warn. Never waits on it:
/// `xdg-open` can stay in the foreground for as long as a freshly started browser runs.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = tokio::process::Command::new(opener);
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Own process group, so Ctrl+C on the dashboard doesn't also close the browser.
    #[cfg(unix)]
    command.process_group(0);
    match command.spawn() {
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(error) => tracing::warn!(%error, opener, "cannot open a browser; open {url} yourself"),
    }
}

/// Resolves on SIGINT or SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                tracing::warn!(%err, "cannot install SIGTERM handler; only Ctrl+C stops the server");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
